use core::fmt;

use orchard::ValuePool;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// A scalar was non-canonical or zero.
    InvalidScalar,
    /// A point was non-canonical, off the curve, or the identity.
    InvalidPoint,
    /// `nk` or `rivk` was non-canonical, or together they yield an invalid viewing key.
    InvalidViewingKey,
    /// The two public shares sum to the identity.
    DegenerateJointKey,
    /// A proof of knowledge of a share did not verify.
    InvalidProof,
    /// The secret shares do not combine to the joint account's `ak`.
    ShareMismatch,
    /// None of the given keys authorizes this spend.
    UnsignedSpend {
        pool: ValuePool,
        index: usize,
    },
    /// The PCZT contains no spend awaiting a signature.
    NothingToSign,
    SweepIntent(String),
    Pczt(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InvalidScalar => f.write_str("scalar is non-canonical or zero"),
            Error::InvalidPoint => {
                f.write_str("point is non-canonical, off the curve, or the identity")
            }
            Error::InvalidViewingKey => f.write_str("invalid nullifier key or ivk randomness"),
            Error::DegenerateJointKey => f.write_str("public shares sum to the identity"),
            Error::InvalidProof => f.write_str("proof of knowledge does not verify"),
            Error::ShareMismatch => f.write_str("secret shares do not match the joint key"),
            Error::UnsignedSpend { pool, index } => {
                write!(f, "no key authorizes {pool:?} action {index}")
            }
            Error::NothingToSign => f.write_str("PCZT has no spend awaiting a signature"),
            Error::SweepIntent(e) => write!(f, "sweep authorization: {e}"),
            Error::Pczt(e) => write!(f, "PCZT error: {e}"),
        }
    }
}

impl std::error::Error for Error {}
