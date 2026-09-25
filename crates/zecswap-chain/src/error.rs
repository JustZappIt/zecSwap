use core::fmt::Display;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid configuration: {0}")]
    Config(String),
    #[error("lightwalletd: {0}")]
    Lightwalletd(#[from] tonic::Status),
    #[error("lightwalletd connection: {0}")]
    Connection(#[from] tonic::transport::Error),
    #[error("lightwalletd rejected the transaction ({code}): {message}")]
    Rejected { code: i32, message: String },
    #[error("wallet database: {0}")]
    Database(#[from] zcash_client_sqlite::error::SqliteClientError),
    #[error("wallet: {0}")]
    Wallet(String),
    #[error("settlement contract: {0}")]
    Contract(String),
    #[error(transparent)]
    Swap(#[from] zecswap_core::Error),
}

impl Error {
    pub(crate) fn wallet(e: impl Display) -> Self {
        Error::Wallet(e.to_string())
    }

    pub(crate) fn contract(e: impl Display) -> Self {
        Error::Contract(e.to_string())
    }
}
