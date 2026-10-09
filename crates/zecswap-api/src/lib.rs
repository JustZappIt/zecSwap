//! The maker's quote API and the relayer's API on the wire. A maker only proposes here:
//! wallets check every term against the contract before depositing.

use alloy_primitives::{Address, B256};
use serde::{Deserialize, Serialize};
use zecswap_core::{PublicShare, ShareProof, ViewingKeys};

pub mod relayer;
pub mod reverse;
#[cfg(feature = "server")]
pub mod server;
pub mod service;
pub mod tokens;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuoteRequest {
    pub units: u32,
    /// The swap's `user`: the account paid, or for a payout into Railgun, the swap's own key.
    pub payout: Address,
    /// For a payout into Railgun, the commitment to the note it pays.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payout_note: Option<B256>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Quote {
    pub quote_id: B256,
    /// The account that opens the swap, which its id binds.
    pub maker: Address,
    #[serde(with = "bytes64")]
    pub maker_share: PublicShare,
    #[serde(with = "bytes64")]
    pub maker_proof: ShareProof,
    pub chain_id: u64,
    pub contract: Address,
    pub token: Address,
    /// Token base units paid out, as a decimal string.
    #[serde(with = "decimal")]
    pub amount: u128,
    pub deposit_zat: u64,
    /// Unix time until which the maker accepts the quote.
    pub expires_at: u64,
    /// Token base units of the maker's own gas and Zcash fee on this swap, at the prices of the
    /// quote: in `depositZat` on top of the amount (ZEC to USDC), or kept from the ZEC paid
    /// (USDC to ZEC). Absent where the maker charges none.
    #[serde(
        default,
        with = "optional_decimal",
        skip_serializing_if = "Option::is_none"
    )]
    pub network_cost: Option<u128>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Acceptance {
    #[serde(with = "bytes64")]
    pub user_share: PublicShare,
    #[serde(with = "bytes64")]
    pub user_proof: ShareProof,
    #[serde(with = "bytes64")]
    pub viewing_keys: ViewingKeys,
    /// Where the maker takes tokens, the one its accept spends asked back: an RFC 9578
    /// `blinded_msg` (base64url, 256 bytes) under the maker's return key, for the maker's
    /// challenge on the day of the accept. The maker signs it once the user pays in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_request: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Accepted {
    pub swap_id: B256,
    /// The deadlines the swap opens with, which the maker picks: from `t0` the user may claim
    /// without `ready`, from `t1` the maker may refund a `Ready` swap. They complete the terms
    /// the wallet checks on-chain.
    pub t0: u64,
    pub t1: u64,
}

/// `GET /v1/swaps/{id}`: a forward swap as the maker has it. Everything else about it is
/// read from the chain.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub swap_id: B256,
    /// The accept's token handed back, once the user has paid in: the RFC 9578 `blind_sig`
    /// (base64url) of its `tokenRequest`. Only the blinding secret turns it into a token.
    pub token_return: Option<String>,
}

/// A swap's terms as `open` committed to them, field for field the contract's `Terms`. The
/// contract stores only their hash, so every request that acts on a swap carries them, and
/// the contract refuses terms that don't hash to it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Terms {
    pub maker: Address,
    pub token: Address,
    /// Token base units, as a decimal string.
    #[serde(with = "decimal")]
    pub amount: u128,
    #[serde(with = "bytes64")]
    pub maker_key: PublicShare,
    #[serde(with = "bytes64")]
    pub user_key: PublicShare,
    pub user: Address,
    pub t0: u64,
    pub t1: u64,
    /// Zero for a swap that pays `user`'s balance rather than a Railgun note.
    pub payout_note: B256,
}

impl From<&zecswap_core::Terms> for Terms {
    fn from(terms: &zecswap_core::Terms) -> Self {
        Self {
            maker: terms.maker.into(),
            token: terms.token.into(),
            amount: terms.amount,
            maker_key: terms.maker_share,
            user_key: terms.user_share,
            user: terms.user.into(),
            t0: terms.t0,
            t1: terms.t1,
            payout_note: terms.payout_note.into(),
        }
    }
}

impl From<&Terms> for zecswap_core::Terms {
    fn from(terms: &Terms) -> Self {
        Self {
            maker: terms.maker.into(),
            token: terms.token.into(),
            amount: terms.amount,
            maker_share: terms.maker_key,
            user_share: terms.user_key,
            user: terms.user.into(),
            t0: terms.t0,
            t1: terms.t1,
            payout_note: terms.payout_note.0,
        }
    }
}

/// 64-byte values as `0x` hex, parsed into their checked types.
mod bytes64 {
    use alloy_primitives::FixedBytes;
    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use zecswap_core::{Error, PublicShare, ShareProof, ViewingKeys};

    pub(crate) trait Bytes64: Sized {
        fn encode(&self) -> [u8; 64];
        fn decode(bytes: &[u8; 64]) -> Result<Self, Error>;
    }

    impl Bytes64 for PublicShare {
        fn encode(&self) -> [u8; 64] {
            self.to_affine_bytes()
        }
        fn decode(bytes: &[u8; 64]) -> Result<Self, Error> {
            Self::from_affine_bytes(bytes)
        }
    }

    impl Bytes64 for ShareProof {
        fn encode(&self) -> [u8; 64] {
            self.to_bytes()
        }
        fn decode(bytes: &[u8; 64]) -> Result<Self, Error> {
            Ok(Self::from_bytes(*bytes))
        }
    }

    impl Bytes64 for ViewingKeys {
        fn encode(&self) -> [u8; 64] {
            self.to_bytes()
        }
        fn decode(bytes: &[u8; 64]) -> Result<Self, Error> {
            Self::from_bytes(bytes)
        }
    }

    pub(crate) fn serialize<T: Bytes64, S: Serializer>(value: &T, s: S) -> Result<S::Ok, S::Error> {
        FixedBytes(value.encode()).serialize(s)
    }

    pub(crate) fn deserialize<'de, T: Bytes64, D: Deserializer<'de>>(d: D) -> Result<T, D::Error> {
        T::decode(&FixedBytes::<64>::deserialize(d)?.0).map_err(D::Error::custom)
    }
}

/// `u128` as a decimal string, which JSON numbers can't hold exactly.
pub(crate) mod decimal {
    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer, Serializer};

    pub(crate) fn serialize<S: Serializer>(value: &u128, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(value)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u128, D::Error> {
        String::deserialize(d)?.parse().map_err(D::Error::custom)
    }
}

pub(crate) mod optional_decimal {
    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer, Serializer};

    pub(crate) fn serialize<S: Serializer>(value: &Option<u128>, s: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(value) => s.collect_str(value),
            None => s.serialize_none(),
        }
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u128>, D::Error> {
        Option::<String>::deserialize(d)?
            .map(|value| value.parse().map_err(D::Error::custom))
            .transpose()
    }
}
