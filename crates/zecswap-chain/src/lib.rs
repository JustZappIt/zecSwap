//! Chain adapters for ZecSwap participants: a Zcash light wallet that watches and sweeps
//! joint accounts, and a client for the settlement contract on an EVM chain.

mod error;
pub mod evm;
pub mod zcash;

pub use error::Error;
