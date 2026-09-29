//! The user side of a ZecSwap, step by step as a wallet runs it. The maker's API only
//! proposes; nothing is deposited until the contract shows what was promised.

mod api;
mod user;

pub use api::{MakerApi, RelayerApi};
pub use user::{CLAIM_MARGIN, MAX_TIME_TO_T0, MIN_TIME_TO_T0, Paid, Route, User, UserSwap};
pub mod reverse;
