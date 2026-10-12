//! USD prices of the assets the bridge moves: ZEC, USDC, and ETH for gas. A `Feed` keeps the
//! latest from providers asked in order, so one provider's outage doesn't leave the bridge
//! without a price; `History` tells what one was worth at a past time, to value what was
//! recorded after the fact. The maker prices quotes with them, the relayer its sponsored sends.

mod feed;
mod history;
#[cfg(any(test, feature = "stand-in"))]
pub mod stand_in;

use serde::{Deserialize, Serialize};

pub use feed::{Feed, Keys, Prices, Quote, Status};
pub use history::{CANDLE, History, Unpriced};

pub const ALCHEMY_PRICES: &str = "https://api.g.alchemy.com/prices/v1/tokens";
const CMC_QUOTES: &str = "https://pro-api.coinmarketcap.com/v3/cryptocurrency/quotes/latest";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Asset {
    Zec,
    Usdc,
    /// Values gas only: never part of a quote.
    Eth,
}

impl Asset {
    pub fn symbol(self) -> &'static str {
        match self {
            Self::Zec => "ZEC",
            Self::Usdc => "USDC",
            Self::Eth => "ETH",
        }
    }

    fn cmc_id(self) -> u64 {
        match self {
            Self::Zec => 1437,
            Self::Usdc => 3408,
            Self::Eth => 1027,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
pub enum Provider {
    #[serde(rename = "coinmarketcap")]
    CoinMarketCap,
    /// The one that also keeps history.
    #[serde(rename = "alchemy")]
    Alchemy,
}

impl Provider {
    pub fn name(self) -> &'static str {
        match self {
            Self::CoinMarketCap => "coinmarketcap",
            Self::Alchemy => "alchemy",
        }
    }

    /// How errors name it.
    fn label(self) -> &'static str {
        match self {
            Self::CoinMarketCap => "CMC",
            Self::Alchemy => "Alchemy",
        }
    }
}

/// A provider's answer, whole, up to 256 KiB. Errors name `label` and a status, never the body:
/// providers' bodies, like requests, can carry keys.
pub async fn read(request: reqwest::RequestBuilder, label: &str) -> Result<Vec<u8>, String> {
    let mut response = request
        .send()
        .await
        .map_err(|_| format!("{label} request failed or timed out"))?;
    if !response.status().is_success() {
        return Err(format!("{label} HTTP {}", response.status().as_u16()));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| format!("{label} response body unavailable"))?
    {
        if body.len().saturating_add(chunk.len()) > 256 * 1024 {
            return Err(format!("{label} response too large"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}
