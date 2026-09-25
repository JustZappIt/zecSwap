//! The maker's quote API on the wire. A maker only proposes here: wallets check every term
//! against the contract before depositing.

use alloy_primitives::{Address, B256};
use serde::{Deserialize, Serialize};
use zecswap_core::{PublicShare, ShareProof, ViewingKeys};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuoteRequest {
    pub units: u32,
    pub payout: Address,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Quote {
    pub quote_id: B256,
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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Accepted {
    pub swap_id: B256,
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
mod decimal {
    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer, Serializer};

    pub(crate) fn serialize<S: Serializer>(value: &u128, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(value)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u128, D::Error> {
        String::deserialize(d)?.parse().map_err(D::Error::custom)
    }
}
