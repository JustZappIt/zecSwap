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
}
