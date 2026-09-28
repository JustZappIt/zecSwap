//! Split-key primitives for atomic swaps of shielded ZEC against an EVM contract.
//!
//! The deposit address's spend authorizing key is `±(e + z)`: the maker holds `e`, the
//! user holds `z`. The settlement contract verifies whichever half is revealed against its
//! public share on Pallas, so a party only ever learns both halves after the contract has
//! paid its counterparty.

mod auth;
mod curve;
mod error;
mod joint;
mod seed;
mod share;
mod signer;
mod transcript;

pub use auth::{AuthKey, Domain, ReverseOpen, signer};
pub use error::Error;
pub use joint::{JointAccount, SpendKey, ViewingKeys};
pub use seed::{UserSwapKeys, derive_maker_share, derive_user_keys};
pub use share::{PublicShare, SecretShare, ShareProof};
pub use signer::{sign_pczt, sign_pczt_bytes};
pub use transcript::{Payout, SwapContext};
pub use zcash_protocol::consensus::NetworkType;
