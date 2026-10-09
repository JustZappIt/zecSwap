use serde::Deserialize;
pub use zecswap_prices::Provider;

const ZATOSHIS_PER_ZEC: u128 = 100_000_000;
const BASIS_POINTS: u128 = 10_000;

/// Fixed denominations: a user sells ZEC for a whole number of `unit`s of the token, so the
/// payout amounts on Base don't fingerprint individual swaps.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pricing {
    /// Token base units per whole ZEC, before the spread.
    #[serde(default)]
    pub price_per_zec: u128,
    pub spread_bps: u16,
    /// Token base units per denomination.
    pub unit: u128,
    pub max_units: u32,
    /// When enabled, fixed prices are ignored, including during provider outages.
    #[serde(default)]
    pub market: Option<MarketConfig>,
    /// The maker's own gas and Zcash fee on each swap, charged on top of the spread at the gas
    /// price and market prices of its quote. Needs `market`.
    #[serde(default)]
    pub costs: Option<Costs>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Costs {
    /// Gas of the maker's sends on a swap from ZEC to USDC: its open and its ready.
    pub forward_gas: u64,
    /// On a swap from USDC to ZEC: its claim lock and its claim.
    pub reverse_gas: u64,
    /// Its Zcash fee on either: a forward swap's sweep, or a reverse swap's deposit.
    pub zcash_fee_zat: u64,
    /// The margin on top, in basis points, for gas prices that rise before the sends land.
    #[serde(default)]
    pub margin_bps: u32,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarketConfig {
    pub token_decimals: u8,
    pub refresh_seconds: u64,
    pub max_age_seconds: u64,
    /// Tried in order on each refresh: the first to answer prices quotes.
    pub providers: Vec<Provider>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Terms {
    /// Token base units the user receives.
    pub amount: u128,
    /// Zatoshis the user deposits, rounded up in the maker's favour.
    pub deposit_zat: u64,
}

impl Pricing {
    /// A swap from USDC to ZEC: the user's `units` buy ZEC at the price less the spread, less
    /// `cost` (token base units), rounded down; none if the cost takes it all.
    pub fn reverse_terms(&self, units: u32, cost: u128) -> Option<Terms> {
        if units == 0
            || units > self.max_units
            || self.price_per_zec == 0
            || self.spread_bps >= 10_000
        {
            return None;
        }
        let amount = self.unit.checked_mul(units.into())?;
        if amount >= (1u128 << 120) {
            return None;
        }
        let deposit = amount
            .checked_mul(BASIS_POINTS - u128::from(self.spread_bps))?
            .checked_sub(cost.checked_mul(BASIS_POINTS)?)?
            .checked_mul(ZATOSHIS_PER_ZEC)?
            / self.price_per_zec.checked_mul(BASIS_POINTS)?;
        (deposit > 0).then_some(Terms {
            amount,
            deposit_zat: deposit.try_into().ok()?,
        })
    }

    /// A swap from ZEC to USDC: the user deposits ZEC for `units` and `cost` (token base units)
    /// at the price less the spread, rounded up.
    pub fn terms(&self, units: u32, cost: u128) -> Option<Terms> {
        if units == 0 || units > self.max_units {
            return None;
        }
        let amount = self.unit.checked_mul(units.into())?;
        let net_price = self
            .price_per_zec
            .checked_mul(BASIS_POINTS.checked_sub(self.spread_bps.into())?)?;
        if net_price == 0 {
            return None;
        }
        let deposit = amount
            .checked_add(cost)?
            .checked_mul(ZATOSHIS_PER_ZEC * BASIS_POINTS)?
            .div_ceil(net_price);
        Some(Terms {
            amount,
            deposit_zat: deposit.try_into().ok()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pricing() -> Pricing {
        Pricing {
            price_per_zec: 40_000_000,
            spread_bps: 100,
            unit: 50_000_000,
            max_units: 20,
            market: None,
            costs: None,
        }
    }

    #[test]
    fn deposit_covers_the_amount_at_the_net_price() {
        let terms = pricing().terms(1, 0).unwrap();
        assert_eq!(terms.amount, 50_000_000);
        // 50 / (40 × 0.99) ZEC, rounded up to the next zatoshi.
        assert_eq!(terms.deposit_zat, 126_262_627);
        assert_eq!(pricing().terms(3, 0).unwrap().amount, 150_000_000);
    }

    #[test]
    fn rejects_out_of_range_units_and_spreads() {
        assert_eq!(pricing().terms(0, 0), None);
        assert_eq!(pricing().terms(21, 0), None);
        let all_spread = Pricing {
            spread_bps: 10_000,
            ..pricing()
        };
        assert_eq!(all_spread.terms(1, 0), None);
        let free = Pricing {
            price_per_zec: 0,
            ..pricing()
        };
        assert_eq!(free.terms(1, 0), None);
    }

    #[test]
    fn reverse_rounds_zec_down_and_keeps_spread_in_the_makers_favour() {
        let terms = pricing().reverse_terms(1, 0).unwrap();
        assert_eq!(terms.amount, 50_000_000);
        assert_eq!(terms.deposit_zat, 123_750_000);
        assert!(terms.deposit_zat < pricing().terms(1, 0).unwrap().deposit_zat);
        assert_eq!(pricing().reverse_terms(0, 0), None);
        assert_eq!(pricing().reverse_terms(21, 0), None);
        assert_eq!(
            Pricing {
                spread_bps: 10_000,
                ..pricing()
            }
            .reverse_terms(1, 0),
            None
        );
        assert_eq!(
            Pricing {
                price_per_zec: 0,
                ..pricing()
            }
            .reverse_terms(1, 0),
            None
        );
        assert_eq!(
            Pricing {
                unit: u128::MAX,
                ..pricing()
            }
            .reverse_terms(2, 0),
            None
        );
    }

    #[test]
    fn a_swaps_network_cost_is_charged_on_top_of_the_spread_both_ways() {
        // A cost of 2: the deposit covers 52 at the net price, 52 / (40 × 0.99) ZEC rounded up.
        let forward = pricing().terms(1, 2_000_000).unwrap();
        assert_eq!(
            (forward.amount, forward.deposit_zat),
            (50_000_000, 131_313_132)
        );
        // 50 × 0.99 − 2 = 47.5 of ZEC at 40: 1.1875 ZEC.
        assert_eq!(
            pricing().reverse_terms(1, 2_000_000).unwrap().deposit_zat,
            118_750_000
        );
        // A cost the swap can't carry leaves nothing to quote.
        assert_eq!(pricing().reverse_terms(1, 49_500_000), None);
        assert!(pricing().reverse_terms(1, 49_499_999).is_some());
    }
}
