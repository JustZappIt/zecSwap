//! Chain adapters for ZecSwap participants: a Zcash light wallet that watches and sweeps
//! joint accounts, and a client for the Base settlement contract.

pub mod base;
mod error;
pub mod zcash;

pub use error::Error;
