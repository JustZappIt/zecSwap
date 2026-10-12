//! The receiving side of a Railgun wallet, as Railgun's own wallets implement it: keys and
//! `0zk` addresses derived from a BIP-39 seed, the shield notes that pay them, and the
//! transaction outputs that do.
//!
//! A swap pays out by shielding into the user's Railgun balance, so the user's device builds
//! the note it will be paid with. Railgun accepts any ciphertext, and a note its receiver cannot
//! decrypt is lost, so the construction here is checked against Railgun's engine
//! (`tests/engine.rs`, and `engine/check.cjs` for notes built here). A relayer sending users'
//! transactions reads its fee from their first outputs, as the engine's broadcasters do.

mod address;
mod babyjubjub;
mod keys;
mod note;

use core::fmt;

pub use keys::{Keys, Receiver};
pub use note::{OutputCiphertext, Received, ShieldCiphertext, ShieldNote};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// A note built for this wallet did not decrypt with its own viewing key.
    UnopenableNote,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::UnopenableNote => f.write_str("a note built for this wallet does not open"),
        }
    }
}

impl std::error::Error for Error {}
