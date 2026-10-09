use alloy_primitives::Address;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ErrorCode {
    InvalidRequest,
    Rejected,
    UnknownQuote,
    UnknownSwap,
    Unavailable,
    WatchtowerUnavailable,
    Internal,
    NotFound,
    MethodNotAllowed,
    /// Spend a token from the issuer the `WWW-Authenticate` challenge names (RFC 9577).
    TokenRequired,
    /// A note the transaction spends is spent, or a transaction already sent spends it.
    AlreadySpent,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorResponse {
    pub code: ErrorCode,
    pub error: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ZcashNetwork {
    Mainnet,
    Testnet,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MakerInfo {
    pub api_version: u32,
    pub maker: Address,
    pub chain_id: u64,
    pub contract: Address,
    pub token: Address,
    pub zcash_network: ZcashNetwork,
    pub reverse_enabled: bool,
    /// Where accepts take tokens, the key the maker hands them back under (base64url SPKI):
    /// one for everyone, which wallets pin.
    pub token_return_key: Option<String>,
}
